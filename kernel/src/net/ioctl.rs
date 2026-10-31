// SPDX-License-Identifier: BSD-2-Clause
//! Interface ioctls on a socket (`ifioctl`): the generic ones any socket
//! takes (`SIOCGIFFLAGS`, `SIOCGIFMTU`, `SIOCGIFINDEX`, `SIOCGIFCONF`,
//! `SIOCGIFGROUP`, ...) and, on an `AF_INET` or `AF_INET6` socket, its
//! family's address requests (`in_control`, `in6_control`); and an
//! internet socket's own (`soo_ioctl`'s `FIONREAD`, `SIOCATMARK`).
//!
//! Behavior reference: sys/net/if.c (`ifioctl`, `ifhwioctl`, `ifconf`,
//! `if_getgroup`), sys/net/if_loop.c (`lo_ioctl`), sys/netinet/in.c
//! (`in_control`, `in_aifaddr_ioctl`, `in_difaddr_ioctl`),
//! sys/netinet6/in6.c (`in6_control`), sys/netinet6/nd6.c
//! (`nd6_ioctl`'s `SIOCGIFINFO_IN6`), sys/kern/sys_socket.c (`soo_ioctl`).

use alloc::vec::Vec;

use abi::errno::Errno;
use abi::inet::{AF_INET, AF_INET6};
use abi::ioctl::{FIOASYNC, FIONBIO, FIONREAD, FIONSPACE, FIONWRITE};
use abi::net::{IFNAMSIZ, iff, nd6_iff, sio};
use inet::addr::{In6, from_v4, v4_of};
use inet::ifnet::{INFINITE, Ifaddr, Ifnet, LOMTU, Net};

use super::curvnet;
use inet::{Family, Socket};
use sched::Sleepable;

use crate::cred::Priv;
use crate::fs::priv_check;
use crate::uaccess;

/// `LOMAXMTU`.
const LOMAXMTU: u32 = 131_072;

/// Whether `req` is one `ifioctl` takes (group `i`).
#[must_use]
pub(crate) fn is_interface_request(req: u32) -> bool {
    (req >> 8) & 0xff == u32::from(b'i')
}

fn put_int(arg: &mut [u8], v: usize) {
    let v = i32::try_from(v).unwrap_or(i32::MAX);
    if arg.len() >= 4 {
        arg[..4].copy_from_slice(&v.to_ne_bytes());
    }
}

/// An internet socket's own requests (`soo_ioctl`).
pub(crate) fn soioctl(so: &Socket, req: u32, arg: &mut [u8]) -> Result<usize, Errno> {
    match req {
        FIONBIO | FIOASYNC => Ok(0),
        FIONREAD => {
            put_int(arg, so.nread());
            Ok(0)
        }
        FIONWRITE => {
            put_int(arg, so.nwrite());
            Ok(0)
        }
        FIONSPACE => {
            put_int(arg, so.send_space());
            Ok(0)
        }
        sio::SIOCATMARK => {
            put_int(arg, usize::from(so.at_mark()));
            Ok(0)
        }
        // `in6_control`'s `in6_src_ioctl`.
        sio::SIOCAADDRCTL_POLICY | sio::SIOCDADDRCTL_POLICY if so.family == Family::Inet6 => {
            priv_check(Priv::NetinetAddrctl6)?;
            policy(req == sio::SIOCAADDRCTL_POLICY, arg)
        }
        _ => Err(Errno::ENOTTY),
    }
}

/// The address selection policy table (`addrsel_policytab`), which
/// `ip6addrctl(8)` installs; source selection here has only `lo0`'s
/// addresses to choose from, so it is kept but not consulted.
static POLICY: sync::SpinLock<Vec<abi::net::In6Addrpolicy>> = sync::SpinLock::new(Vec::new());

/// `ND6_IFF_DEFAULTIF`'s interface (`V_nd6_defifindex`).
static DEFIFINDEX: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// `add_addrsel_policyent` and `delete_addrsel_policyent`.
fn policy(add: bool, arg: &[u8]) -> Result<usize, Errno> {
    let mut p = abi::net::In6Addrpolicy::default();
    let b = crate::uaccess::bytes_of_mut(&mut p);
    let n = b.len().min(arg.len());
    b[..n].copy_from_slice(&arg[..n]);
    // `ADDR_LABEL_NOTAPP`, and a mask that is not a prefix.
    if p.label == -1 {
        return Err(Errno::EINVAL);
    }
    let m = p.addrmask.sin6_addr;
    let plen = plen_of(&m);
    if mask6(plen) != m {
        return Err(Errno::EINVAL);
    }
    let same = |e: &abi::net::In6Addrpolicy| {
        e.addr.sin6_addr == p.addr.sin6_addr && e.addrmask.sin6_addr == p.addrmask.sin6_addr
    };
    let mut t = POLICY.lock();
    if add {
        if t.iter().any(same) {
            return Err(Errno::EEXIST);
        }
        p.r#use = 0;
        t.push(p);
    } else {
        let i = t.iter().position(same).ok_or(Errno::ESRCH)?;
        t.remove(i);
    }
    Ok(0)
}

/// `net.inet6.ip6.addrctlpolicy`'s records.
pub(crate) fn policies(out: &mut Vec<u8>) {
    for p in POLICY.lock().iter() {
        out.extend_from_slice(crate::uaccess::bytes_of(p));
    }
}

/// The interface name at the start of a request (`ifr_name`).
fn name_of(arg: &[u8]) -> &[u8] {
    let n = &arg[..IFNAMSIZ.min(arg.len())];
    let end = n.iter().position(|&b| b == 0).unwrap_or(n.len());
    &n[..end]
}

fn ifp<'a>(n: &'a Net, arg: &[u8]) -> Result<&'a Ifnet, Errno> {
    n.by_name(name_of(arg)).ok_or(Errno::ENXIO)
}

fn rd32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4)
        .and_then(|x| x.try_into().ok())
        .map_or(0, u32::from_ne_bytes)
}

fn wr32(b: &mut [u8], at: usize, v: u32) {
    if let Some(x) = b.get_mut(at..at + 4) {
        x.copy_from_slice(&v.to_ne_bytes());
    }
}

/// `ifru_*` starts here in a `struct ifreq` and `struct in6_ifreq`.
const IFRU: usize = IFNAMSIZ;

/// The `sockaddr_in` of an IPv4 address.
fn sin(a: [u8; 4]) -> Vec<u8> {
    inet::addr::sockaddr_in(a, 0)
}

fn mask4(plen: u8) -> [u8; 4] {
    let m: u32 = if plen == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(plen.min(32)))
    };
    m.to_be_bytes()
}

fn mask6(plen: u8) -> In6 {
    inet::ifnet::masked(Family::Inet6, &[0xff; 16], plen)
}

/// The prefix length of a netmask's bits (`in_mask2len`).
fn plen_of(m: &[u8]) -> u8 {
    let mut n = 0u8;
    for &b in m {
        if b == 0xff {
            n += 8;
        } else {
            n += u8::try_from(b.leading_ones()).unwrap_or(0);
            break;
        }
    }
    n
}

/// `ifioctl`, for a socket of `family` (`None`: a UNIX-domain or netlink
/// one, which has no address requests).
pub(crate) fn ifioctl(
    s: &Sleepable,
    req: u32,
    arg: &mut [u8],
    family: Option<Family>,
) -> Result<usize, Errno> {
    let _ = s;
    match req {
        sio::SIOCGIFCONF => return ifconf(arg),
        sio::SIOCSIFVNET | sio::SIOCSIFRVNET => {
            priv_check(Priv::NetSetifvnet)?;
            vmove(req == sio::SIOCSIFVNET, arg)?;
            return Ok(0);
        }
        sio::SIOCIFCREATE | sio::SIOCIFCREATE2 => {
            priv_check(Priv::NetIfcreate)?;
            let name = net::clone::create(&curvnet(), name_of(arg), &super::clone_ctx())?;
            // The name it got (`if_clone_create` copies it back).
            arg[..IFNAMSIZ].fill(0);
            let n = name.len().min(IFNAMSIZ - 1);
            arg[..n].copy_from_slice(&name.as_bytes()[..n]);
            return Ok(0);
        }
        sio::SIOCIFDESTROY => {
            priv_check(Priv::NetIfdestroy)?;
            net::clone::destroy(&curvnet(), name_of(arg))?;
            return Ok(0);
        }
        sio::SIOCIFGCLONERS => return cloners(arg),
        _ => {}
    }
    match req {
        sio::SIOCGIFINDEX
        | sio::SIOCGIFFLAGS
        | sio::SIOCGIFMETRIC
        | sio::SIOCGIFMTU
        | sio::SIOCGIFCAP
        | sio::SIOCGIFFIB
        | sio::SIOCGIFPHYS
        | sio::SIOCGIFDESCR
        | sio::SIOCGIFDATA
        | sio::SIOCGIFGROUP => return get(req, arg),
        sio::SIOCSIFFLAGS
        | sio::SIOCSIFMETRIC
        | sio::SIOCSIFMTU
        | sio::SIOCSIFCAP
        | sio::SIOCSIFFIB
        | sio::SIOCSIFDESCR => return set(req, arg),
        sio::SIOCGIFMEDIA | sio::SIOCGIFXMEDIA => return media(arg),
        // `lo_ioctl` takes none of these.
        sio::SIOCGIFSTATUS | sio::SIOCGIFCAPNV | sio::SIOCGLANPCP | sio::SIOCGTUNFIB => {
            ifp(&curvnet().net.lock(), arg)?;
            return Err(Errno::EINVAL);
        }
        _ => {}
    }
    match family {
        Some(Family::Inet) => in_control(req, arg),
        Some(Family::Inet6) => in6_control(req, arg),
        // `pr_control_notsupp`.
        None => {
            ifp(&curvnet().net.lock(), arg)?;
            Err(Errno::EOPNOTSUPP)
        }
    }
}

/// `if_vmove_loan` (`loan`) and `if_vmove_reclaim`: moves the interface
/// into the VNET of jail `ifr_jid` (below this one), or back from it.
fn vmove(loan: bool, arg: &[u8]) -> Result<(), Errno> {
    let jid = i32::from_ne_bytes(arg[IFRU..IFRU + 4].try_into().unwrap_or([0; 4]));
    let me = curvnet();
    let name = name_of(arg);
    if loan && me.net.lock().by_name(name).is_none() {
        return Err(Errno::ENXIO);
    }
    let pr = crate::prison::find_child(&crate::prison::current(), jid).ok_or(Errno::ENXIO)?;
    let there = pr.vnet();
    if !loan && there.net.lock().by_name(name).is_none() {
        return Err(Errno::ENXIO);
    }
    if alloc::sync::Arc::ptr_eq(there, &me) {
        return Err(Errno::EEXIST);
    }
    if loan {
        net::clone::vmove(&me, name, there)
    } else {
        net::clone::vmove(there, name, &me)
    }
}

/// `if_clone_list`: the cloners' names, `IFNAMSIZ` bytes each, as many
/// as `ifcr_count` asks for, and how many there are.
fn cloners(arg: &mut [u8]) -> Result<usize, Errno> {
    let count = i32::from_ne_bytes(arg[4..8].try_into().unwrap_or([0; 4]));
    let buf = u64::from_ne_bytes(arg[8..16].try_into().unwrap_or([0; 8]));
    let count = usize::try_from(count).map_err(|_| Errno::EINVAL)?;
    let all = net::clone::CLONERS;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        reason = "two"
    )]
    arg[0..4].copy_from_slice(&(all.len() as i32).to_ne_bytes());
    if buf != 0 {
        let n = count.min(all.len());
        let mut out = alloc::vec![0u8; n * IFNAMSIZ];
        for (i, c) in all.iter().take(n).enumerate() {
            out[i * IFNAMSIZ..i * IFNAMSIZ + c.len()].copy_from_slice(c.as_bytes());
        }
        crate::uaccess::copy_out(buf, &out).map_err(|_| Errno::EFAULT)?;
    }
    Ok(0)
}

/// `ifmedia_ioctl`'s `SIOCGIFMEDIA` (`struct ifmediareq`): the current,
/// active, and status words, and the supported ones into `ifm_ulist` as
/// far as `ifm_count` allows (`E2BIG` if not all fit).
fn media(arg: &mut [u8]) -> Result<usize, Errno> {
    let m = {
        let vn = curvnet();
        let n = vn.net.lock();
        let i = ifp(&n, arg)?;
        i.link
            .0
            .as_ref()
            .and_then(|l| l.media())
            .ok_or(Errno::EINVAL)?
    };
    let count = i32::from_ne_bytes(arg[32..36].try_into().unwrap_or([0; 4]));
    let ulist = u64::from_ne_bytes(arg[40..48].try_into().unwrap_or([0; 8]));
    if count < 0 {
        return Err(Errno::EINVAL);
    }
    for (at, v) in [
        (16, m.current),
        (20, 0),
        (24, m.status),
        (28, m.active),
        (32, 1),
    ] {
        arg[at..at + 4].copy_from_slice(&v.to_ne_bytes());
    }
    if count > 0 {
        crate::uaccess::copy_out(ulist, &m.supported.to_ne_bytes()).map_err(|_| Errno::EFAULT)?;
    }
    Ok(0)
}

/// `ifhwioctl`'s reads.
fn get(req: u32, arg: &mut [u8]) -> Result<usize, Errno> {
    let vn = curvnet();
    let n = vn.net.lock();
    let i = ifp(&n, arg)?;
    match req {
        sio::SIOCGIFINDEX => {
            arg[IFRU..IFRU + 2].copy_from_slice(&i.index.to_ne_bytes());
        }
        sio::SIOCGIFFLAGS => {
            // `ifr_flags` (low 16 bits) and `ifr_flagshigh`.
            let f = i.flags;
            arg[IFRU..IFRU + 2]
                .copy_from_slice(&u16::try_from(f & 0xffff).unwrap_or(0).to_ne_bytes());
            arg[IFRU + 2..IFRU + 4]
                .copy_from_slice(&u16::try_from(f >> 16).unwrap_or(0).to_ne_bytes());
        }
        sio::SIOCGIFMETRIC => wr32(arg, IFRU, i.metric),
        sio::SIOCGIFMTU => wr32(arg, IFRU, i.mtu),
        sio::SIOCGIFCAP => {
            wr32(arg, IFRU, i.capabilities);
            wr32(arg, IFRU + 4, i.capenable);
        }
        sio::SIOCGIFFIB | sio::SIOCGIFPHYS => wr32(arg, IFRU, 0),
        sio::SIOCGIFDESCR => {
            // `ifr_buffer`: its length and a user pointer.
            let Some(d) = i.description.clone() else {
                return Err(Errno::ENOMSG);
            };
            drop(n);
            let len = rd32(arg, IFRU) as usize;
            let ptr = u64::from_ne_bytes(arg[IFRU + 8..IFRU + 16].try_into().unwrap_or([0; 8]));
            let mut out = d;
            out.push(0);
            if len < out.len() {
                wr32(arg, IFRU, u32::try_from(out.len()).unwrap_or(0));
                return Err(Errno::ENAMETOOLONG);
            }
            uaccess::copy_out(ptr, &out).map_err(|_| Errno::EFAULT)?;
            wr32(arg, IFRU, u32::try_from(out.len()).unwrap_or(0));
        }
        sio::SIOCGIFDATA => {
            let d = inet::rtmsg::if_data(i);
            drop(n);
            let ptr = u64::from_ne_bytes(arg[IFRU..IFRU + 8].try_into().unwrap_or([0; 8]));
            uaccess::copy_out(ptr, crate::uaccess::bytes_of(&d)).map_err(|_| Errno::EFAULT)?;
        }
        sio::SIOCGIFGROUP => {
            // `struct ifgroupreq`: a length, and a user array of
            // `struct ifg_req` (one name each).
            let groups = i.groups.clone();
            drop(n);
            let need = groups.len() * IFNAMSIZ;
            let len = rd32(arg, IFRU) as usize;
            if len == 0 {
                wr32(arg, IFRU, u32::try_from(need).unwrap_or(0));
                return Ok(0);
            }
            let ptr = u64::from_ne_bytes(arg[IFRU + 8..IFRU + 16].try_into().unwrap_or([0; 8]));
            let mut at = 0;
            for g in &groups {
                if len - at < IFNAMSIZ {
                    return Err(Errno::EINVAL);
                }
                let mut r = [0u8; IFNAMSIZ];
                let b = g.as_bytes();
                r[..b.len().min(IFNAMSIZ - 1)].copy_from_slice(&b[..b.len().min(IFNAMSIZ - 1)]);
                uaccess::copy_out(ptr + at as u64, &r).map_err(|_| Errno::EFAULT)?;
                at += IFNAMSIZ;
            }
            wr32(arg, IFRU, u32::try_from(at).unwrap_or(0));
        }
        _ => return Err(Errno::ENOTTY),
    }
    Ok(0)
}

/// `ifhwioctl`'s changes, each with its privilege.
fn set(req: u32, arg: &mut [u8]) -> Result<usize, Errno> {
    let name = name_of(arg).to_vec();
    match req {
        sio::SIOCSIFFLAGS => {
            priv_check(Priv::NetSetifflags)?;
            let lo = u32::from(u16::from_ne_bytes([arg[IFRU], arg[IFRU + 1]]));
            let hi = u32::from(u16::from_ne_bytes([arg[IFRU + 2], arg[IFRU + 3]]));
            curvnet().net.lock().set_flags(&name, lo | (hi << 16))?;
        }
        sio::SIOCSIFMETRIC => {
            priv_check(Priv::NetSetifmetric)?;
            let v = rd32(arg, IFRU);
            curvnet()
                .net
                .lock()
                .with_iface(&name, |i| i.metric = v)
                .ok_or(Errno::ENXIO)?;
        }
        sio::SIOCSIFMTU => {
            priv_check(Priv::NetSetifmtu)?;
            let v = rd32(arg, IFRU);
            // `lo_ioctl`: up to `LOMAXMTU`; `ifhwioctl`: at least 72.
            if !(72..=LOMAXMTU).contains(&v) {
                ifp(&curvnet().net.lock(), arg)?;
                return Err(Errno::EINVAL);
            }
            curvnet()
                .net
                .lock()
                .with_iface(&name, |i| i.mtu = v)
                .ok_or(Errno::ENXIO)?;
        }
        sio::SIOCSIFCAP => {
            priv_check(Priv::NetSetifcap)?;
            let want = rd32(arg, IFRU);
            let vn = curvnet();
            let mut n = vn.net.lock();
            let i = n.by_name(&name).ok_or(Errno::ENXIO)?;
            if want & !i.capabilities != 0 {
                return Err(Errno::EINVAL);
            }
            n.with_iface(&name, |i| i.capenable = want);
        }
        sio::SIOCSIFFIB => {
            ifp(&curvnet().net.lock(), arg)?;
            if rd32(arg, IFRU) != 0 {
                return Err(Errno::EINVAL);
            }
        }
        sio::SIOCSIFDESCR => {
            priv_check(Priv::NetSetifdescr)?;
            let len = rd32(arg, IFRU) as usize;
            let ptr = u64::from_ne_bytes(arg[IFRU + 8..IFRU + 16].try_into().unwrap_or([0; 8]));
            // `ifdescr_maxlen` 1024; a length of 1 or less clears it.
            if len > 1024 {
                return Err(Errno::ENAMETOOLONG);
            }
            let d = if len <= 1 {
                None
            } else {
                let mut b = alloc::vec![0u8; len];
                uaccess::copy_in(&mut b, ptr).map_err(|_| Errno::EFAULT)?;
                let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
                b.truncate(end);
                Some(b)
            };
            curvnet()
                .net
                .lock()
                .with_iface(&name, |i| i.description = d)
                .ok_or(Errno::ENXIO)?;
        }
        _ => return Err(Errno::ENOTTY),
    }
    Ok(0)
}

/// `ifconf`: an `ifreq` per address of each interface (its link address
/// first), one whose address is longer than a `sockaddr` growing to fit.
fn ifconf(arg: &mut [u8]) -> Result<usize, Errno> {
    let len = usize::try_from(i32::from_ne_bytes(arg[0..4].try_into().unwrap_or([0; 4])))
        .map_err(|_| Errno::EINVAL)?;
    let ptr = u64::from_ne_bytes(arg[8..16].try_into().unwrap_or([0; 8]));
    let mut out = Vec::new();
    {
        let vn = curvnet();
        let n = vn.net.lock();
        for i in &n.ifaces {
            let mut entries = alloc::vec![inet::rtmsg::link_sockaddr(i)];
            for a in &i.addrs {
                entries.push(match a.family {
                    Family::Inet => sin(v4_of(&a.addr)),
                    Family::Inet6 => inet::addr::sockaddr_in6(
                        &a.addr,
                        0,
                        if inet::addr::is_linklocal6(&a.addr) {
                            u32::from(i.index)
                        } else {
                            0
                        },
                    ),
                });
            }
            for sa in entries {
                let mut r = alloc::vec![0u8; IFNAMSIZ];
                let b = i.name.as_bytes();
                r[..b.len()].copy_from_slice(b);
                r.extend_from_slice(&sa);
                if sa.len() < 16 {
                    r.resize(IFNAMSIZ + 16, 0);
                }
                out.extend(r);
            }
        }
    }
    let n = out.len().min(len);
    // Only whole records (`ifconf` stops at the first that does not fit).
    uaccess::copy_out(ptr, &out[..n]).map_err(|_| Errno::EFAULT)?;
    arg[0..4].copy_from_slice(&i32::try_from(n).unwrap_or(0).to_ne_bytes());
    Ok(0)
}

/// The address of a family in `ifr_addr`, if the `sockaddr` there is of
/// it.
fn req_addr4(arg: &[u8], at: usize) -> Option<[u8; 4]> {
    let sa = arg.get(at..at + 16)?;
    (sa[1] == AF_INET).then(|| [sa[4], sa[5], sa[6], sa[7]])
}

fn req_addr6(arg: &[u8], at: usize) -> Option<In6> {
    let sa = arg.get(at..at + 28)?;
    (sa[1] == AF_INET6).then(|| {
        let mut a = [0u8; 16];
        a.copy_from_slice(&sa[8..24]);
        a
    })
}

fn first4(i: &Ifnet, want: Option<[u8; 4]>) -> Option<&Ifaddr> {
    i.addrs.iter().find(|a| {
        a.family == Family::Inet && want.is_none_or(|w| w == [0; 4] || v4_of(&a.addr) == w)
    })
}

/// `in_control` for an `AF_INET` socket.
fn in_control(req: u32, arg: &mut [u8]) -> Result<usize, Errno> {
    let name = name_of(arg).to_vec();
    match req {
        sio::SIOCGIFADDR | sio::SIOCGIFNETMASK | sio::SIOCGIFDSTADDR | sio::SIOCGIFBRDADDR => {
            let vn = curvnet();
            let n = vn.net.lock();
            let i = ifp(&n, arg)?;
            let a = first4(i, req_addr4(arg, IFRU)).ok_or(Errno::EADDRNOTAVAIL)?;
            let sa = match req {
                sio::SIOCGIFADDR => sin(v4_of(&a.addr)),
                sio::SIOCGIFNETMASK => sin(mask4(a.plen)),
                sio::SIOCGIFDSTADDR => {
                    if i.flags & iff::POINTOPOINT == 0 {
                        return Err(Errno::EINVAL);
                    }
                    sin(v4_of(&a.dst.unwrap_or(a.addr)))
                }
                _ => {
                    if i.flags & iff::BROADCAST == 0 {
                        return Err(Errno::EINVAL);
                    }
                    sin(v4_of(&a.broadcast.unwrap_or([0; 16])))
                }
            };
            arg[IFRU..IFRU + 16].copy_from_slice(&sa);
            Ok(0)
        }
        sio::SIOCAIFADDR => {
            priv_check(Priv::NetAddifaddr)?;
            // `struct ifaliasreq`: address, broadcast (or destination),
            // mask.
            let addr = req_addr4(arg, 16).ok_or(Errno::EINVAL)?;
            let dst = req_addr4(arg, 32).filter(|d| *d != [0; 4]);
            let mask = arg
                .get(48 + 4..48 + 8)
                .filter(|_| arg[49] == AF_INET || arg[48] != 0);
            let vn = curvnet();
            let mut n = vn.net.lock();
            let i = ifp(&n, arg)?;
            let loopback = i.flags & iff::LOOPBACK != 0;
            let p2p = i.flags & iff::POINTOPOINT != 0;
            // With no mask, the old one or the classful default.
            let plen = match mask {
                Some(m) => plen_of(m),
                None => first4(i, Some(addr)).map_or(if loopback { 8 } else { 24 }, |a| a.plen),
            };
            let a = Ifaddr {
                family: Family::Inet,
                addr: from_v4(addr),
                plen,
                dst: if loopback || p2p {
                    Some(from_v4(dst.filter(|_| p2p).unwrap_or(addr)))
                } else {
                    None
                },
                broadcast: dst.filter(|_| !p2p && !loopback).map(from_v4),
                flags6: 0,
                vltime: INFINITE,
                pltime: INFINITE,
                vhid: rd32(arg, 64),
            };
            n.add_addr(&name, a)?;
            Ok(0)
        }
        sio::SIOCDIFADDR => {
            priv_check(Priv::NetDelifaddr)?;
            let addr = req_addr4(arg, IFRU).map(from_v4);
            curvnet()
                .net
                .lock()
                .delete_addr(&name, Family::Inet, addr.as_ref())
                .map(|_| 0)
        }
        _ => {
            ifp(&curvnet().net.lock(), arg)?;
            Err(Errno::EINVAL)
        }
    }
}

fn find6<'a>(i: &'a Ifnet, a: Option<&In6>) -> Option<&'a Ifaddr> {
    i.addrs
        .iter()
        .find(|x| x.family == Family::Inet6 && a.is_none_or(|a| x.addr == *a))
}

/// `in6_control` (and `nd6_ioctl`) for an `AF_INET6` socket.
fn in6_control(req: u32, arg: &mut [u8]) -> Result<usize, Errno> {
    let name = name_of(arg).to_vec();
    match req {
        sio::SIOCGDEFIFACE_IN6 => {
            let v = DEFIFINDEX.load(core::sync::atomic::Ordering::Relaxed);
            arg[IFNAMSIZ..IFNAMSIZ + 8].copy_from_slice(&v.to_ne_bytes());
            Ok(0)
        }
        sio::SIOCSDEFIFACE_IN6 => {
            priv_check(Priv::NetinetNd6)?;
            let v = u64::from_ne_bytes(arg[IFNAMSIZ..IFNAMSIZ + 8].try_into().unwrap_or([0; 8]));
            if v != 0
                && curvnet()
                    .net
                    .lock()
                    .by_index(u16::try_from(v).map_err(|_| Errno::EINVAL)?)
                    .is_none()
            {
                return Err(Errno::EINVAL);
            }
            // Relaxed: one word, read alone.
            DEFIFINDEX.store(v, core::sync::atomic::Ordering::Relaxed);
            Ok(0)
        }
        sio::SIOCGIFINFO_IN6 => {
            let vn = curvnet();
            let n = vn.net.lock();
            let i = ifp(&n, arg)?;
            let ndi = abi::net::NdIfinfo {
                linkmtu: i.mtu,
                maxmtu: LOMTU,
                basereachable: 30_000,
                reachable: 30_000,
                retrans: 1000,
                flags: i.nd6_flags,
                recalctm: 7200,
                chlim: 64,
                initialized: 1,
                ..abi::net::NdIfinfo::default()
            };
            let b = crate::uaccess::bytes_of(&ndi);
            arg[IFNAMSIZ..IFNAMSIZ + b.len()].copy_from_slice(b);
            Ok(0)
        }
        sio::SIOCSIFINFO_IN6 => {
            priv_check(Priv::NetinetNd6)?;
            let flags = rd32(arg, IFNAMSIZ + 20);
            let vn = curvnet();
            let mut n = vn.net.lock();
            let i = ifp(&n, arg)?;
            let index = i.index;
            let was = i.nd6_flags;
            let up = i.flags & iff::UP != 0;
            n.with_iface(&name, |i| i.nd6_flags = flags);
            // IPv6 enabled again, or link-local addresses wanted, on an
            // interface that is up (`nd6_ioctl`).
            if up
                && ((was & nd6_iff::IFDISABLED != 0 && flags & nd6_iff::IFDISABLED == 0)
                    || (was & nd6_iff::AUTO_LINKLOCAL == 0 && flags & nd6_iff::AUTO_LINKLOCAL != 0))
            {
                n.in6_ifattach(index);
            }
            Ok(0)
        }
        sio::SIOCGIFAFLAG_IN6
        | sio::SIOCGIFALIFETIME_IN6
        | sio::SIOCGIFADDR_IN6
        | sio::SIOCGIFNETMASK_IN6
        | sio::SIOCGIFDSTADDR_IN6 => {
            let want = req_addr6(arg, IFRU);
            let vn = curvnet();
            let n = vn.net.lock();
            let i = ifp(&n, arg)?;
            let a = find6(i, want.as_ref()).ok_or(Errno::EADDRNOTAVAIL)?;
            let u = &mut arg[IFRU..];
            u.fill(0);
            match req {
                sio::SIOCGIFAFLAG_IN6 => u[..4].copy_from_slice(&a.flags6.to_ne_bytes()),
                sio::SIOCGIFALIFETIME_IN6 => {
                    // `in6_addrlifetime`: no expiry for infinite lifetimes.
                    u[16..20].copy_from_slice(&a.vltime.to_ne_bytes());
                    u[20..24].copy_from_slice(&a.pltime.to_ne_bytes());
                }
                sio::SIOCGIFADDR_IN6 => {
                    let z = if inet::addr::is_linklocal6(&a.addr) {
                        u32::from(i.index)
                    } else {
                        0
                    };
                    u[..28].copy_from_slice(&inet::addr::sockaddr_in6(&a.addr, 0, z));
                }
                sio::SIOCGIFNETMASK_IN6 => {
                    u[..28].copy_from_slice(&inet::addr::sockaddr_in6(&mask6(a.plen), 0, 0));
                }
                _ => {
                    if i.flags & iff::POINTOPOINT == 0 {
                        return Err(Errno::EINVAL);
                    }
                    u[..28].copy_from_slice(&inet::addr::sockaddr_in6(
                        &a.dst.unwrap_or(a.addr),
                        0,
                        0,
                    ));
                }
            }
            Ok(0)
        }
        sio::SIOCGIFSTAT_IN6 | sio::SIOCGIFSTAT_ICMP6 => {
            ifp(&curvnet().net.lock(), arg)?;
            arg[IFRU..].fill(0);
            Ok(0)
        }
        sio::SIOCAIFADDR_IN6 => {
            priv_check(Priv::NetAddifaddr)?;
            // `struct in6_aliasreq`.
            let addr = req_addr6(arg, 16).ok_or(Errno::EINVAL)?;
            let mask = arg.get(72 + 8..72 + 24).map(plen_of);
            let flags6 = i32::from_ne_bytes(arg[100..104].try_into().unwrap_or([0; 4]));
            let vl = rd32(arg, 120);
            let pl = rd32(arg, 124);
            let plen = match mask {
                Some(p) if arg[73] == AF_INET6 => p,
                _ => 128,
            };
            if plen > 128 {
                return Err(Errno::EINVAL);
            }
            curvnet().net.lock().add_addr(
                &name,
                Ifaddr {
                    family: Family::Inet6,
                    addr,
                    plen,
                    dst: None,
                    broadcast: None,
                    flags6,
                    vltime: if vl == 0 { INFINITE } else { vl },
                    pltime: if pl == 0 { INFINITE } else { pl },
                    vhid: rd32(arg, 136),
                },
            )?;
            Ok(0)
        }
        sio::SIOCDIFADDR_IN6 => {
            priv_check(Priv::NetDelifaddr)?;
            let addr = req_addr6(arg, IFRU).ok_or(Errno::EADDRNOTAVAIL)?;
            curvnet()
                .net
                .lock()
                .delete_addr(&name, Family::Inet6, Some(&addr))
                .map(|_| 0)
        }
        _ => {
            ifp(&curvnet().net.lock(), arg)?;
            Err(Errno::EINVAL)
        }
    }
}
