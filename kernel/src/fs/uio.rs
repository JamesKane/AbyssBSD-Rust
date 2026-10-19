// SPDX-License-Identifier: BSD-2-Clause
//! A transfer to or from user memory: the buffers of `read`, `write`, and
//! their vector forms, as a [`vfs::Uio`].
//!
//! A fault part-way leaves what was moved moved; the caller reports the
//! count if there is one, as FreeBSD's `dofileread` does for an
//! interrupted transfer.

use alloc::vec::Vec;

use abi::errno::Errno;
use abi::uio::{IOV_MAX, Iovec};

use crate::uaccess::{self, Plain, UserPtr};

// SAFETY: two `u64`s, no padding, any bits valid.
unsafe impl Plain for Iovec {}

/// User buffers, consumed from the front.
pub(crate) struct UserIo {
    iov: Buffers,
    /// The first buffer not yet used up.
    at: usize,
    resid: usize,
}

/// The buffers: one (`read`, `write`) kept inline, so the common calls
/// allocate nothing, or a vector's.
enum Buffers {
    One(Iovec),
    Many(Vec<Iovec>),
}

impl Buffers {
    fn get_mut(&mut self, i: usize) -> Option<&mut Iovec> {
        match self {
            Self::One(v) => (i == 0).then_some(v),
            Self::Many(v) => v.get_mut(i),
        }
    }
}

impl UserIo {
    /// One buffer of `len` bytes at `base`.
    pub(crate) fn one(base: u64, len: u64) -> Result<Self, Errno> {
        let resid = usize::try_from(len)
            .ok()
            .filter(|&l| isize::try_from(l).is_ok())
            .ok_or(Errno::EINVAL)?;
        Ok(Self {
            iov: Buffers::One(Iovec {
                iov_base: base,
                iov_len: len,
            }),
            at: 0,
            resid,
        })
    }

    /// The `iovcnt` buffers described at `iov`.
    pub(crate) fn vector(iov: u64, iovcnt: u64) -> Result<Self, Errno> {
        let n = usize::try_from(iovcnt)
            .ok()
            .filter(|&n| n <= IOV_MAX)
            .ok_or(Errno::EINVAL)?;
        let mut v = Vec::new();
        v.try_reserve_exact(n).map_err(|_| Errno::ENOMEM)?;
        let mut resid: usize = 0;
        for i in 0..n {
            let at = iov + (i * size_of::<Iovec>()) as u64;
            let e = UserPtr::<Iovec>::new(at)
                .read()
                .map_err(|_| Errno::EFAULT)?;
            resid = usize::try_from(e.iov_len)
                .ok()
                .and_then(|l| resid.checked_add(l))
                .filter(|&t| isize::try_from(t).is_ok())
                .ok_or(Errno::EINVAL)?;
            v.push(e);
        }
        Ok(Self {
            iov: Buffers::Many(v),
            at: 0,
            resid,
        })
    }

    /// Calls `f` with each user range in turn, up to `len` bytes, and
    /// consumes what it reports done.
    fn each(
        &mut self,
        len: usize,
        mut f: impl FnMut(u64, usize, usize) -> Result<(), Errno>,
    ) -> Result<usize, Errno> {
        let want = len.min(self.resid);
        let mut done = 0;
        while done < want {
            let Some(v) = self.iov.get_mut(self.at) else {
                break;
            };
            let n = usize::try_from(v.iov_len)
                .unwrap_or(usize::MAX)
                .min(want - done);
            if n == 0 {
                self.at += 1;
                continue;
            }
            f(v.iov_base, done, n)?;
            v.iov_base += n as u64;
            v.iov_len -= n as u64;
            self.resid -= n;
            done += n;
        }
        Ok(done)
    }
}

impl vfs::Uio for UserIo {
    fn resid(&self) -> usize {
        self.resid
    }

    fn copy_out(&mut self, data: &[u8]) -> Result<usize, Errno> {
        self.each(data.len(), |user, from, n| {
            uaccess::copy_out(user, &data[from..from + n]).map_err(|_| Errno::EFAULT)
        })
    }

    fn copy_in(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
        let len = buf.len();
        self.each(len, |user, from, n| {
            uaccess::copy_in(&mut buf[from..from + n], user).map_err(|_| Errno::EFAULT)
        })
    }
}
