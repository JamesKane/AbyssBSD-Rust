// SPDX-License-Identifier: BSD-2-Clause
//! `write` and `writev`. Until there are file descriptors (P4.5),
//! descriptors 1 and 2 are the console and every other is `EBADF`.

use abi::errno::Errno;
use abi::uio::{IOV_MAX, Iovec};

use super::SysResult;
use crate::uaccess::{self, Plain, UserPtr};

// SAFETY: two `u64`s, no padding, any bits valid.
unsafe impl Plain for Iovec {}

fn console(fd: u64) -> Result<(), Errno> {
    if fd == 1 || fd == 2 {
        Ok(())
    } else {
        Err(Errno::EBADF)
    }
}

/// Copies `len` bytes at `buf` to the console.
fn put(buf: u64, len: u64) -> Result<(), Errno> {
    let len = usize::try_from(len).map_err(|_| Errno::EINVAL)?;
    let mut chunk = [0u8; 256];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(chunk.len());
        let at = buf.checked_add(done as u64).ok_or(Errno::EFAULT)?;
        uaccess::copy_in(&mut chunk[..n], at).map_err(|_| Errno::EFAULT)?;
        crate::console::write_bytes(&chunk[..n]);
        done += n;
    }
    Ok(())
}

/// `write(fd, buf, nbyte)`.
pub(super) fn write(fd: u64, buf: u64, nbyte: u64) -> SysResult {
    console(fd)?;
    if i64::try_from(nbyte).is_err() {
        return Err(Errno::EINVAL);
    }
    put(buf, nbyte)?;
    Ok(nbyte)
}

/// `writev(fd, iov, iovcnt)`.
pub(super) fn writev(fd: u64, iov: u64, iovcnt: u64) -> SysResult {
    console(fd)?;
    let n = usize::try_from(iovcnt)
        .ok()
        .filter(|&n| n <= IOV_MAX)
        .ok_or(Errno::EINVAL)?;
    let mut total: u64 = 0;
    for i in 0..n {
        let v = UserPtr::<Iovec>::new(iov + (i * size_of::<Iovec>()) as u64)
            .read()
            .map_err(|_| Errno::EFAULT)?;
        total = total
            .checked_add(v.iov_len)
            .filter(|&t| i64::try_from(t).is_ok())
            .ok_or(Errno::EINVAL)?;
        put(v.iov_base, v.iov_len)?;
    }
    Ok(total)
}
