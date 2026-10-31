// SPDX-License-Identifier: BSD-2-Clause
//! `setsockopt` and `getsockopt`: the socket level's options and the
//! local protocol's.
//!
//! Behavior reference: sys/kern/uipc_socket.c (`sosetopt`, `sogetopt`),
//! sys/kern/uipc_sockbuf.c (`sbsetopt`), sys/kern/uipc_usrreq.c
//! (`uipc_ctloutput`).

use core::sync::atomic::Ordering;

use alloc::vec::Vec;

use abi::errno::Errno;
use abi::socket::{
    AF_UNIX, LOCAL_CREDS, LOCAL_CREDS_PERSISTENT, LOCAL_PEERCRED, SO_ACCEPTCONN, SO_BINTIME,
    SO_BROADCAST, SO_DEBUG, SO_DOMAIN, SO_DONTROUTE, SO_ERROR, SO_FIB, SO_KEEPALIVE, SO_LINGER,
    SO_LISTENINCQLEN, SO_LISTENQLEN, SO_LISTENQLIMIT, SO_MAX_PACING_RATE, SO_NO_DDP, SO_NO_OFFLOAD,
    SO_NOSIGPIPE, SO_OOBINLINE, SO_PROTOCOL, SO_RCVBUF, SO_RCVLOWAT, SO_RCVTIMEO, SO_RERROR,
    SO_REUSEADDR, SO_REUSEPORT, SO_REUSEPORT_LB, SO_SNDBUF, SO_SNDLOWAT, SO_SNDTIMEO, SO_TIMESTAMP,
    SO_TS_CLOCK, SO_TS_CLOCK_MAX, SO_TYPE, SO_USELOOPBACK, SO_USER_COOKIE, SOL_LOCAL, SOL_SOCKET,
};

use crate::{Socket, flags};

/// The boolean options, each its own `so_options` bit.
const BOOL_OPTS: &[i32] = &[
    SO_DEBUG,
    SO_KEEPALIVE,
    SO_DONTROUTE,
    SO_USELOOPBACK,
    SO_BROADCAST,
    SO_REUSEADDR,
    SO_REUSEPORT,
    SO_REUSEPORT_LB,
    SO_OOBINLINE,
    SO_TIMESTAMP,
    SO_BINTIME,
    SO_NOSIGPIPE,
    SO_NO_DDP,
    SO_NO_OFFLOAD,
    SO_RERROR,
];

/// `sooptcopyin` of an `int`: `EINVAL` if shorter.
fn int(val: &[u8]) -> Result<i32, Errno> {
    let b: [u8; 4] = val
        .get(..4)
        .and_then(|b| b.try_into().ok())
        .ok_or(Errno::EINVAL)?;
    Ok(i32::from_ne_bytes(b))
}

fn i64_at(val: &[u8], at: usize) -> i64 {
    val.get(at..at + 8)
        .and_then(|b| b.try_into().ok())
        .map_or(0, i64::from_ne_bytes)
}

const NANOS: u64 = 1_000_000_000;

impl Socket {
    /// `sosetopt`.
    ///
    /// # Errors
    ///
    /// `EINVAL` for a short or bad value, `EDOM` for an out-of-range
    /// time, `ENOBUFS` for a buffer too big, `ENOPROTOOPT` for an unknown
    /// option.
    pub fn setopt(&self, level: i32, name: i32, val: &[u8]) -> Result<(), Errno> {
        if level != SOL_SOCKET {
            return self.local_setopt(level, name, val);
        }
        let mut i = self.inner.lock();
        match name {
            SO_LINGER => {
                if val.len() < 8 {
                    return Err(Errno::EINVAL);
                }
                let onoff = int(&val[..4])?;
                let linger = int(&val[4..8])?;
                // `hz` 1000.
                if linger < 0 || linger > i32::from(u16::MAX) || linger > i32::MAX / 1000 {
                    return Err(Errno::EDOM);
                }
                i.linger = linger;
                if onoff != 0 {
                    i.options |= SO_LINGER;
                } else {
                    i.options &= !SO_LINGER;
                }
            }
            n if BOOL_OPTS.contains(&n) => {
                if int(val)? != 0 {
                    i.options |= n;
                } else {
                    i.options &= !n;
                }
            }
            SO_USER_COOKIE => i.user_cookie = int(val)?.cast_unsigned(),
            SO_SNDBUF | SO_RCVBUF | SO_SNDLOWAT | SO_RCVLOWAT => {
                let v = int(val)?;
                if v < 1 {
                    return Err(Errno::EINVAL);
                }
                let cc = v.cast_unsigned();
                if name == SO_SNDBUF {
                    if !self.reserve_snd(&mut i, cc) {
                        return Err(Errno::ENOBUFS);
                    }
                    return Ok(());
                }
                let sb = if name == SO_SNDLOWAT {
                    &mut i.snd
                } else {
                    &mut i.rcv
                };
                if name == SO_RCVBUF {
                    if !sb.reserve(cc) {
                        return Err(Errno::ENOBUFS);
                    }
                } else {
                    sb.lowat = cc.min(sb.hiwat);
                }
            }
            SO_SNDTIMEO | SO_RCVTIMEO => {
                if val.len() < 16 {
                    return Err(Errno::EINVAL);
                }
                let (sec, usec) = (i64_at(val, 0), i64_at(val, 8));
                if sec < 0 || !(0..1_000_000).contains(&usec) {
                    return Err(Errno::EDOM);
                }
                let ns = if sec > i64::from(i32::MAX) {
                    u64::MAX
                } else {
                    sec.cast_unsigned() * NANOS + usec.cast_unsigned() * 1000
                };
                if name == SO_SNDTIMEO {
                    self.snd_timeo.store(ns, Ordering::Relaxed);
                } else {
                    self.rcv_timeo.store(ns, Ordering::Relaxed);
                }
            }
            SO_TS_CLOCK => {
                let v = int(val)?;
                if !(0..=SO_TS_CLOCK_MAX).contains(&v) {
                    return Err(Errno::EINVAL);
                }
                i.ts_clock = v;
            }
            SO_MAX_PACING_RATE => i.pacing = int(val)?.cast_unsigned(),
            // Only FIB 0 (no routing here).
            SO_FIB => {
                if int(val)? != 0 {
                    return Err(Errno::EINVAL);
                }
            }
            _ => return Err(Errno::ENOPROTOOPT),
        }
        Ok(())
    }

    /// `uipc_ctloutput`'s set.
    fn local_setopt(&self, level: i32, name: i32, val: &[u8]) -> Result<(), Errno> {
        if level != SOL_LOCAL {
            return Err(Errno::EINVAL);
        }
        let (bit, other) = match name {
            LOCAL_CREDS => (flags::WANTCRED_ONESHOT, flags::WANTCRED_ALWAYS),
            LOCAL_CREDS_PERSISTENT => (flags::WANTCRED_ALWAYS, flags::WANTCRED_ONESHOT),
            _ => return Err(Errno::ENOPROTOOPT),
        };
        let on = int(val)? != 0;
        let mut i = self.inner.lock();
        if on {
            if i.flags & other != 0 {
                return Err(Errno::EINVAL);
            }
            i.flags |= bit;
        } else {
            i.flags &= !bit;
        }
        Ok(())
    }

    /// `sogetopt`: the value, which the caller copies out to the size the
    /// user gave.
    ///
    /// # Errors
    ///
    /// `ENOPROTOOPT` for an unknown option; `SOL_LOCAL`'s errors.
    pub fn getopt(&self, level: i32, name: i32) -> Result<Vec<u8>, Errno> {
        let i32v = |v: i32| v.to_ne_bytes().to_vec();
        if level != SOL_SOCKET {
            if level != SOL_LOCAL {
                return Err(Errno::EINVAL);
            }
            return match name {
                LOCAL_PEERCRED => {
                    let x = self.peercred()?;
                    let mut b = Vec::with_capacity(size_of_val(&x));
                    b.extend_from_slice(&x.cr_version.to_ne_bytes());
                    b.extend_from_slice(&x.cr_uid.to_ne_bytes());
                    b.extend_from_slice(&x.cr_ngroups.to_ne_bytes());
                    b.extend_from_slice(&[0, 0]);
                    for g in x.cr_groups {
                        b.extend_from_slice(&g.to_ne_bytes());
                    }
                    b.extend_from_slice(&[0; 4]);
                    b.extend_from_slice(&x.cr_pid.to_ne_bytes());
                    b.extend_from_slice(&[0; 4]);
                    Ok(b)
                }
                LOCAL_CREDS => Ok(i32v(i32::from(
                    self.inner.lock().flags & flags::WANTCRED_ONESHOT != 0,
                ))),
                LOCAL_CREDS_PERSISTENT => Ok(i32v(i32::from(
                    self.inner.lock().flags & flags::WANTCRED_ALWAYS != 0,
                ))),
                _ => Err(Errno::EOPNOTSUPP),
            };
        }
        let mut i = self.inner.lock();
        let tv = |ns: u64| {
            let mut b = Vec::new();
            let (s, us) = if ns == u64::MAX {
                (i64::MAX, 0)
            } else {
                (
                    (ns / NANOS).cast_signed(),
                    (ns % NANOS / 1000).cast_signed(),
                )
            };
            b.extend_from_slice(&s.to_ne_bytes());
            b.extend_from_slice(&us.to_ne_bytes());
            b
        };
        let (qlimit, qlen) = i.listen.as_ref().map_or((0, 0), |l| {
            (l.qlimit, u32::try_from(l.comp.len()).unwrap_or(0))
        });
        Ok(match name {
            SO_LINGER => {
                let mut b = i32v(i.options & SO_LINGER);
                b.extend_from_slice(&i.linger.to_ne_bytes());
                b
            }
            n if BOOL_OPTS.contains(&n) || n == SO_ACCEPTCONN => i32v(i.options & n),
            SO_FIB => i32v(0),
            SO_DOMAIN => i32v(i32::from(AF_UNIX)),
            SO_TYPE => i32v(self.ty.number().cast_signed()),
            SO_PROTOCOL => i32v(0),
            SO_ERROR => i32v(i.error.take().map_or(0, Errno::get)),
            SO_SNDBUF => i32v(i.snd.hiwat.cast_signed()),
            SO_RCVBUF => i32v(i.rcv.hiwat.cast_signed()),
            SO_SNDLOWAT => i32v(i.snd.lowat.cast_signed()),
            SO_RCVLOWAT => i32v(i.rcv.lowat.cast_signed()),
            SO_SNDTIMEO => tv(self.snd_timeo.load(Ordering::Relaxed)),
            SO_RCVTIMEO => tv(self.rcv_timeo.load(Ordering::Relaxed)),
            SO_LISTENQLIMIT => i32v(qlimit.cast_signed()),
            SO_LISTENQLEN => i32v(qlen.cast_signed()),
            SO_LISTENINCQLEN => i32v(0),
            SO_USER_COOKIE => i32v(i.user_cookie.cast_signed()),
            SO_TS_CLOCK => i32v(i.ts_clock),
            SO_MAX_PACING_RATE => i32v(i.pacing.cast_signed()),
            _ => return Err(Errno::ENOPROTOOPT),
        })
    }
}
