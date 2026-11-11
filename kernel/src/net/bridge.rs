// SPDX-License-Identifier: BSD-2-Clause
//! The bridge's control requests (`SIOCGDRVSPEC`, `SIOCSDRVSPEC`): each
//! `BRDG*` command's argument copied in and out by its size, checked as
//! `bridge_ioctl` checks it, and handed to [`net::bridge::Bridge`].
//!
//! Behavior reference: sys/net/if_bridge.c (`bridge_ioctl`,
//! `bridge_control_table`, `bridge_ioctl_gifs`, `bridge_ioctl_rts`,
//! `bridge_ioctl_gifsstp`).

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::bridge::{
    Ifbareq, Ifbconf, IfbifVlanReq, Ifbpstpreq, Ifbreq, Ifbropreq, Ifbrparam, Ifdrv, cmd,
};
use abi::errno::Errno;
use net::bridge::{Bridge, VlanSet};

use crate::cred::Priv;
use crate::fs::priv_check;
use crate::uaccess::{self, Plain};

// SAFETY: `struct ifdrv` is a name and three 64-bit integers, no padding.
unsafe impl Plain for Ifdrv {}
// SAFETY: integers and byte arrays, its padding explicit fields.
unsafe impl Plain for Ifbreq {}
// SAFETY: two integers, the padding between them a field.
unsafe impl Plain for Ifbconf {}
// SAFETY: integers and byte arrays, its padding explicit fields.
unsafe impl Plain for Ifbareq {}
// SAFETY: one integer.
unsafe impl Plain for Ifbrparam {}
// SAFETY: integers and a `timeval` (two `i64`s), padding explicit.
unsafe impl Plain for Ifbropreq {}
// SAFETY: integers, padding explicit.
unsafe impl Plain for Ifbpstpreq {}
// SAFETY: a name, a byte, explicit padding, and 64-bit words.
unsafe impl Plain for IfbifVlanReq {}

/// `BC_F_COPYIN`, `BC_F_COPYOUT`, `BC_F_SUSER`.
const IN: u8 = 1;
const OUT: u8 = 2;
const SU: u8 = 4;

/// `bridge_control_table`: each command's argument size and flags.
const TABLE: [(usize, u8); 39] = {
    const BREQ: usize = size_of::<Ifbreq>();
    const PARAM: usize = size_of::<Ifbrparam>();
    const CONF: usize = size_of::<Ifbconf>();
    const AREQ: usize = size_of::<Ifbareq>();
    const VLAN: usize = size_of::<IfbifVlanReq>();
    [
        (BREQ, IN | SU),               // ADD
        (BREQ, IN | SU),               // DEL
        (BREQ, IN | OUT),              // GIFFLGS
        (BREQ, IN | SU),               // SIFFLGS
        (PARAM, IN | SU),              // SCACHE
        (PARAM, OUT),                  // GCACHE
        (CONF, IN | OUT),              // GIFS
        (CONF, IN | OUT),              // RTS
        (AREQ, IN | SU),               // SADDR
        (PARAM, IN | SU),              // STO
        (PARAM, OUT),                  // GTO
        (AREQ, IN | SU),               // DADDR
        (BREQ, IN | SU),               // FLUSH
        (PARAM, OUT),                  // GPRI
        (PARAM, IN | SU),              // SPRI
        (PARAM, OUT),                  // GHT
        (PARAM, IN | SU),              // SHT
        (PARAM, OUT),                  // GFD
        (PARAM, IN | SU),              // SFD
        (PARAM, OUT),                  // GMA
        (PARAM, IN | SU),              // SMA
        (BREQ, IN | SU),               // SIFPRIO
        (BREQ, IN | SU),               // SIFCOST
        (BREQ, IN | SU),               // ADDS
        (BREQ, IN | SU),               // DELS
        (size_of::<Ifbropreq>(), OUT), // PARAM
        (PARAM, OUT),                  // GRTE
        (CONF, IN | OUT),              // GIFSSTP
        (PARAM, IN | SU),              // SPROTO
        (PARAM, IN | SU),              // STXHC
        (BREQ, IN | SU),               // SIFAMAX
        (BREQ, IN | SU),               // SIFPVID
        (VLAN, IN | SU),               // SIFVLANSET
        (VLAN, IN | OUT),              // GIFVLANSET
        (PARAM, OUT),                  // GFLAGS
        (PARAM, IN | SU),              // SFLAGS
        (PARAM, OUT),                  // GDEFPVID
        (PARAM, IN | SU),              // SDEFPVID
        (BREQ, IN | SU),               // SIFVLANPROTO
    ]
};

fn name(b: &[u8]) -> &[u8] {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    &b[..end]
}

/// The argument as `T`, from the bytes copied in.
fn arg<T: Plain + Default>(buf: &[u8]) -> T {
    let mut t = T::default();
    let b = uaccess::bytes_of_mut(&mut t);
    let n = b.len().min(buf.len());
    b[..n].copy_from_slice(&buf[..n]);
    t
}

/// `t`'s bytes back into the argument.
fn put<T: Plain>(buf: &mut [u8], t: &T) {
    let b = uaccess::bytes_of(t);
    let n = b.len().min(buf.len());
    buf[..n].copy_from_slice(&b[..n]);
}

/// `records` into the user buffer of `c` as far as whole records fit
/// in `c.len`, which becomes what was written (`bridge_ioctl_gifs` and
/// its siblings).
fn copy_records<T: Plain>(c: &mut Ifbconf, records: &[T]) -> Result<(), Errno> {
    let size = size_of::<T>();
    let n = (c.len as usize / size).min(records.len());
    let mut out = Vec::with_capacity(n * size);
    for r in &records[..n] {
        out.extend_from_slice(uaccess::bytes_of(r));
    }
    c.len = u32::try_from(out.len()).unwrap_or(0);
    uaccess::copy_out(c.buf, &out).map_err(|_| Errno::EFAULT)
}

/// `SIOCGDRVSPEC` (`set` false) and `SIOCSDRVSPEC` on the interface `struct
/// ifdrv` `a` names.
pub(crate) fn drvspec(set: bool, a: &mut [u8]) -> Result<usize, Errno> {
    let ifd: Ifdrv = arg(a);
    let vn = super::curvnet();
    let b: Arc<Bridge> = {
        let n = vn.net.lock();
        let i = n.by_name(name(&ifd.ifd_name)).ok_or(Errno::ENXIO)?;
        // Any other driver's `ether_ioctl` (or `lo_ioctl`) takes none.
        net::bridge::of(i)
            .and_then(Bridge::arc)
            .ok_or(Errno::EINVAL)?
    };
    let (size, flags) = usize::try_from(ifd.ifd_cmd)
        .ok()
        .and_then(|c| TABLE.get(c))
        .copied()
        .ok_or(Errno::EINVAL)?;
    if !set && flags & OUT == 0 || set && flags & OUT != 0 {
        return Err(Errno::EINVAL);
    }
    if flags & SU != 0 {
        priv_check(Priv::NetBridge)?;
    }
    if ifd.ifd_len != size as u64 {
        return Err(Errno::EINVAL);
    }
    let mut buf = alloc::vec![0u8; size];
    if flags & IN != 0 {
        uaccess::copy_in(&mut buf, ifd.ifd_data).map_err(|_| Errno::EFAULT)?;
    }
    command(&vn, &b, ifd.ifd_cmd, &mut buf)?;
    if flags & OUT != 0 {
        uaccess::copy_out(ifd.ifd_data, &buf).map_err(|_| Errno::EFAULT)?;
    }
    Ok(0)
}

#[allow(clippy::too_many_lines, reason = "one arm per command, as the table")]
fn command(vn: &Arc<net::Vnet>, b: &Arc<Bridge>, c: u64, buf: &mut [u8]) -> Result<(), Errno> {
    let param = |buf: &[u8]| arg::<Ifbrparam>(buf);
    match c {
        cmd::ADD => {
            let r: Ifbreq = arg(buf);
            b.add(vn, &mut vn.net.lock(), name(&r.ifbr_ifsname))
        }
        cmd::DEL => {
            let r: Ifbreq = arg(buf);
            b.del(&mut vn.net.lock(), name(&r.ifbr_ifsname))
        }
        cmd::GIFFLGS => {
            let r: Ifbreq = arg(buf);
            let out = b.member(&vn.net.lock(), name(&r.ifbr_ifsname))?;
            put(buf, &out);
            Ok(())
        }
        cmd::SIFFLGS => {
            let r: Ifbreq = arg(buf);
            b.set_member_flags(&vn.net.lock(), name(&r.ifbr_ifsname), r.ifbr_ifsflags)
        }
        cmd::SCACHE => {
            b.set_cache(param(buf).ifbrp_ifbrpu);
            Ok(())
        }
        cmd::GCACHE => {
            put(
                buf,
                &Ifbrparam {
                    ifbrp_ifbrpu: b.cache(),
                },
            );
            Ok(())
        }
        cmd::GIFS => {
            let mut c: Ifbconf = arg(buf);
            let all = b.members(&vn.net.lock());
            if c.len == 0 {
                c.len = u32::try_from(all.len() * size_of::<Ifbreq>()).unwrap_or(u32::MAX);
            } else {
                copy_records(&mut c, &all)?;
            }
            put(buf, &c);
            Ok(())
        }
        cmd::RTS => {
            let mut c: Ifbconf = arg(buf);
            if c.len == 0 {
                return Ok(());
            }
            let all = b.addresses(&vn.net.lock(), super::uptime_s());
            copy_records(&mut c, &all)?;
            put(buf, &c);
            Ok(())
        }
        cmd::SADDR | cmd::DADDR => {
            let r: Ifbareq = arg(buf);
            if c == cmd::DADDR {
                return b.delete_address(&r.ifba_dst, r.ifba_vlan);
            }
            b.set_address(
                &vn.net.lock(),
                name(&r.ifba_ifsname),
                &r.ifba_dst,
                r.ifba_vlan,
                r.ifba_flags,
            )
        }
        cmd::STO => {
            b.set_timeout(param(buf).ifbrp_ifbrpu);
            Ok(())
        }
        cmd::GTO => {
            put(
                buf,
                &Ifbrparam {
                    ifbrp_ifbrpu: b.timeout(),
                },
            );
            Ok(())
        }
        cmd::FLUSH => {
            let r: Ifbreq = arg(buf);
            b.flush(r.ifbr_ifsflags);
            Ok(())
        }
        cmd::GPRI | cmd::GHT | cmd::GFD | cmd::GMA | cmd::GRTE | cmd::GFLAGS | cmd::GDEFPVID => {
            let mut p = Ifbrparam::default();
            match c {
                cmd::GPRI => p.set_int16(b.priority()),
                cmd::GHT => p.set_int8(b.hellotime()),
                cmd::GFD => p.set_int8(b.fwddelay()),
                cmd::GMA => p.set_int8(b.maxage()),
                cmd::GRTE => p.ifbrp_ifbrpu = b.exceeded(),
                cmd::GFLAGS => p.ifbrp_ifbrpu = b.flags(),
                _ => p.set_int16(b.defpvid()),
            }
            put(buf, &p);
            Ok(())
        }
        cmd::SPRI => b.set_priority(param(buf).int16()),
        cmd::SHT => b.set_hellotime(param(buf).int8()),
        cmd::SFD => b.set_fwddelay(param(buf).int8()),
        cmd::SMA => b.set_maxage(param(buf).int8()),
        cmd::SPROTO => b.set_protocol(param(buf).int8()),
        cmd::STXHC => b.set_holdcount(param(buf).int8()),
        cmd::SFLAGS => {
            b.set_flags(param(buf).ifbrp_ifbrpu);
            Ok(())
        }
        cmd::SDEFPVID => b.set_defpvid(param(buf).int16()),
        cmd::SIFPRIO | cmd::SIFCOST | cmd::SIFAMAX | cmd::SIFPVID | cmd::SIFVLANPROTO => {
            let r: Ifbreq = arg(buf);
            let n = vn.net.lock();
            let m = name(&r.ifbr_ifsname);
            match c {
                cmd::SIFPRIO => b.set_port_priority(&n, m, r.ifbr_priority),
                cmd::SIFCOST => b.set_path_cost(&n, m, r.ifbr_path_cost),
                cmd::SIFAMAX => b.set_addrmax(&n, m, r.ifbr_addrmax),
                cmd::SIFPVID => b.set_pvid(&n, m, r.ifbr_pvid),
                _ => b.set_vlanproto(&n, m, r.ifbr_vlanproto),
            }
        }
        cmd::ADDS | cmd::DELS => {
            let r: Ifbreq = arg(buf);
            let n = vn.net.lock();
            if c == cmd::ADDS {
                b.add_span(&n, name(&r.ifbr_ifsname))
            } else {
                b.del_span(&n, name(&r.ifbr_ifsname))
            }
        }
        cmd::PARAM => {
            put(buf, &b.params());
            Ok(())
        }
        cmd::GIFSSTP => {
            // No port takes part in the spanning tree.
            let mut c: Ifbconf = arg(buf);
            c.len = 0;
            put(buf, &c);
            Ok(())
        }
        cmd::SIFVLANSET | cmd::GIFVLANSET => {
            let mut r: IfbifVlanReq = arg(buf);
            let n = vn.net.lock();
            let m = name(&r.bv_ifname);
            if c == cmd::SIFVLANSET {
                return b.set_vlans(&n, m, r.bv_op, &VlanSet(r.bv_set));
            }
            r.bv_set = b.vlans(&n, m)?.0;
            drop(n);
            put(buf, &r);
            Ok(())
        }
        _ => Err(Errno::EINVAL),
    }
}
