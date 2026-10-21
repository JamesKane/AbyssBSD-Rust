// SPDX-License-Identifier: BSD-2-Clause
//! Kernel access to user memory.
//!
//! User addresses arrive as numbers and stay numbers: [`UserPtr`] and
//! [`copy_in`]/[`copy_out`] are the only ways to reach the memory behind
//! them, and every one is fallible. The bounds are checked here; the
//! mapping is checked by the MMU, with faults resolved by the VM or turned
//! into [`Fault`] by the copy routines' fixups (ARCHITECTURE.md §5.2).
//!
//! Behavior reference: copyin(9), copyinstr(9).

#![cfg_attr(not(feature = "ktest"), allow(dead_code))] // the syscalls (P4.2) use it

use alloc::vec::Vec;
use core::marker::PhantomData;
use core::mem::{MaybeUninit, size_of};

use crate::arch;
use crate::mm::space::USER_END;

/// A user address that could not be read or written (`EFAULT`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Fault;

/// Types whose every bit pattern is a valid value and which have no
/// padding, so they can be copied in from, and out to, user memory.
///
/// # Safety
///
/// Implement only for such types: no padding bytes (they would leak kernel
/// memory on the way out), no invalid bit patterns (`bool`, enums,
/// references).
pub(crate) unsafe trait Plain: Copy {}

macro_rules! plain {
    ($($t:ty),*) => { $(
        // SAFETY: integers have neither padding nor invalid patterns.
        unsafe impl Plain for $t {}
    )* };
}
plain!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize);

// SAFETY: an array of plain elements has no padding between them.
unsafe impl<T: Plain, const N: usize> Plain for [T; N] {}

/// The bytes of a [`Plain`] value (for replies built in the kernel).
pub(crate) fn bytes_of<T: Plain>(v: &T) -> &[u8] {
    // SAFETY: a `Plain` value has no padding, so all its bytes are
    // initialized; the slice borrows `v` for its lifetime.
    unsafe { core::slice::from_raw_parts(core::ptr::from_ref(v).cast::<u8>(), size_of::<T>()) }
}

/// Whether `[addr, addr + len)` lies in the user half.
fn in_user(addr: u64, len: usize) -> bool {
    u64::try_from(len)
        .ok()
        .and_then(|l| addr.checked_add(l))
        .is_some_and(|end| end <= USER_END)
}

/// Copies `dst.len()` bytes from user address `src`.
pub(crate) fn copy_in(dst: &mut [u8], src: u64) -> Result<(), Fault> {
    if !in_user(src, dst.len()) {
        return Err(Fault);
    }
    // SAFETY: `dst` is writable for its length, and the user range was
    // checked above.
    unsafe { arch::user::copy_from_user(dst.as_mut_ptr(), src, dst.len()) }.map_err(|_| Fault)
}

/// Copies `src` to user address `dst`.
pub(crate) fn copy_out(dst: u64, src: &[u8]) -> Result<(), Fault> {
    if !in_user(dst, src.len()) {
        return Err(Fault);
    }
    // SAFETY: `src` is readable for its length, and the user range was
    // checked above.
    unsafe { arch::user::copy_to_user(dst, src.as_ptr(), src.len()) }.map_err(|_| Fault)
}

/// Why [`copy_in_str`] failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StrError {
    /// The string runs into unreadable memory (`EFAULT`).
    Fault,
    /// No NUL within the buffer (`ENAMETOOLONG`).
    TooLong,
}

/// Copies a NUL-terminated string from `src` into `buf`; returns its
/// length without the NUL. Reads a page at a time, so a string that ends
/// just before unmapped memory is not a fault.
pub(crate) fn copy_in_str(src: u64, buf: &mut [u8]) -> Result<usize, StrError> {
    let page = vm::PAGE_SIZE;
    let mut done = 0;
    while done < buf.len() {
        let at = src.checked_add(done as u64).ok_or(StrError::Fault)?;
        let to_page = usize::try_from(page - at % page).unwrap_or(usize::MAX);
        let n = to_page.min(buf.len() - done);
        copy_in(&mut buf[done..done + n], at).map_err(|_| StrError::Fault)?;
        if let Some(nul) = buf[done..done + n].iter().position(|&b| b == 0) {
            return Ok(done + nul);
        }
        done += n;
    }
    Err(StrError::TooLong)
}

/// Copies in the NUL-terminated string at `src`, which with its NUL must
/// fit in `max` bytes, without the NUL. It goes in small pieces, so a
/// short string (a path, mostly) costs little whatever `max` is.
pub(crate) fn copy_in_str_vec(src: u64, max: usize) -> Result<Vec<u8>, StrError> {
    const PIECE: usize = 128;
    let page = vm::PAGE_SIZE;
    let mut piece = [0u8; PIECE];
    let mut out = Vec::new();
    let mut done = 0;
    while done < max {
        let at = src
            .checked_add(u64::try_from(done).map_err(|_| StrError::Fault)?)
            .ok_or(StrError::Fault)?;
        let to_page = usize::try_from(page - at % page).unwrap_or(usize::MAX);
        let n = to_page.min(PIECE).min(max - done);
        copy_in(&mut piece[..n], at).map_err(|_| StrError::Fault)?;
        if let Some(nul) = piece[..n].iter().position(|&b| b == 0) {
            out.extend_from_slice(&piece[..nul]);
            return Ok(out);
        }
        out.extend_from_slice(&piece[..n]);
        done += n;
    }
    Err(StrError::TooLong)
}

/// A typed user pointer.
pub(crate) struct UserPtr<T> {
    addr: u64,
    _type: PhantomData<fn() -> T>,
}

impl<T> Clone for UserPtr<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for UserPtr<T> {}

impl<T> core::fmt::Debug for UserPtr<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "UserPtr({:#x})", self.addr)
    }
}

impl<T: Plain> UserPtr<T> {
    pub(crate) const fn new(addr: u64) -> Self {
        Self {
            addr,
            _type: PhantomData,
        }
    }

    /// Reads the value.
    pub(crate) fn read(self) -> Result<T, Fault> {
        let mut v = MaybeUninit::<T>::uninit();
        // SAFETY: the buffer is the value's own storage, `size_of::<T>()`
        // bytes.
        let bytes =
            unsafe { core::slice::from_raw_parts_mut(v.as_mut_ptr().cast::<u8>(), size_of::<T>()) };
        copy_in(bytes, self.addr)?;
        // SAFETY: every byte was written, and any bit pattern is a valid
        // `T` (`Plain`).
        Ok(unsafe { v.assume_init() })
    }

    /// Writes `v`.
    pub(crate) fn write(self, v: &T) -> Result<(), Fault> {
        // SAFETY: `T` has no padding (`Plain`), so every byte is
        // initialized.
        let bytes = unsafe {
            core::slice::from_raw_parts(core::ptr::from_ref(v).cast::<u8>(), size_of::<T>())
        };
        copy_out(self.addr, bytes)
    }
}
