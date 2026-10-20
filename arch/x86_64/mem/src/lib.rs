// SPDX-License-Identifier: BSD-2-Clause
//! `memcpy`, `memmove`, and `memset` for the kernel, replacing the
//! `compiler_builtins` ones (which are weak).
//!
//! Those run every copy as three string instructions (`rep movsb`, `rep
//! movsq`, `rep movsb`), each with a startup cost of tens of cycles, and an
//! overlapping copy backwards with the direction flag set, which no x86
//! CPU accelerates: one byte at a time. The kernel has no SSE, and most of
//! its copies are small (frames, map entries, structures). Here a small
//! copy is a few 8-byte moves; only a large forward one uses `rep movsb`,
//! which CPUs with fast strings run at full speed.
//!
//! The loops' moves are inline assembly, so the compiler cannot recognize
//! them as a copy and call back into these functions.
//!
//! The symbols are defined only for the kernel's target; host tests call
//! the same functions under their Rust names.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
#![cfg(target_arch = "x86_64")]

use core::arch::asm;
use core::ffi::{c_int, c_void};

/// Forward copies and fills at least this long use the string
/// instructions.
const STRING_MIN: usize = 512;

#[inline(always)]
unsafe fn load(p: *const u8) -> u64 {
    let v;
    // SAFETY: the caller's contract: `p..p + 8` is readable.
    unsafe {
        asm!("mov {v}, qword ptr [{p}]", p = in(reg) p, v = out(reg) v,
            options(nostack, readonly, preserves_flags));
    }
    v
}

#[inline(always)]
unsafe fn store(p: *mut u8, v: u64) {
    // SAFETY: the caller's contract: `p..p + 8` is writable.
    unsafe {
        asm!("mov qword ptr [{p}], {v}", p = in(reg) p, v = in(reg) v,
            options(nostack, preserves_flags));
    }
}

/// Copies fewer than 17 bytes, reading all before writing any (so any
/// overlap is fine).
#[inline(always)]
unsafe fn short(dst: *mut u8, src: *const u8, n: usize) {
    // SAFETY: (all arms) both ranges are `n` bytes, valid per the
    // callers' contracts; each access lies within them.
    unsafe {
        match n {
            8.. => {
                let (a, b) = (load(src), load(src.add(n - 8)));
                store(dst, a);
                store(dst.add(n - 8), b);
            }
            4.. => {
                let a = src.cast::<u32>().read_unaligned();
                let b = src.add(n - 4).cast::<u32>().read_unaligned();
                dst.cast::<u32>().write_unaligned(a);
                dst.add(n - 4).cast::<u32>().write_unaligned(b);
            }
            2.. => {
                let a = src.cast::<u16>().read_unaligned();
                let b = src.add(n - 2).cast::<u16>().read_unaligned();
                dst.cast::<u16>().write_unaligned(a);
                dst.add(n - 2).cast::<u16>().write_unaligned(b);
            }
            1 => dst.write(src.read()),
            _ => {}
        }
    }
}

/// Copies `n > 16` bytes upwards. Correct for an overlap with `dst` below
/// `src`: each word is read before anything above it is written, and the
/// last 16 bytes are read first.
unsafe fn forward(dst: *mut u8, src: *const u8, n: usize) {
    // SAFETY: per the callers' contracts, both ranges are `n` bytes.
    unsafe {
        if n >= STRING_MIN {
            // Byte by byte upwards, architecturally, so the same overlap
            // is fine.
            asm!("rep movsb", inout("rcx") n => _, inout("rdi") dst => _,
                inout("rsi") src => _, options(nostack, preserves_flags));
            return;
        }
        let (t0, t1) = (load(src.add(n - 16)), load(src.add(n - 8)));
        let mut off = 0;
        while off < n - 16 {
            store(dst.add(off), load(src.add(off)));
            off += 8;
        }
        store(dst.add(n - 16), t0);
        store(dst.add(n - 8), t1);
    }
}

/// Copies `n > 16` bytes downwards, for an overlap with `dst` above `src`:
/// each word is read before anything below it is written, and the first
/// 16 bytes are read first.
unsafe fn backward(dst: *mut u8, src: *const u8, n: usize) {
    // SAFETY: per the callers' contracts, both ranges are `n` bytes.
    unsafe {
        let (h0, h1) = (load(src), load(src.add(8)));
        let mut off = n - 8;
        while off >= 16 {
            store(dst.add(off), load(src.add(off)));
            off -= 8;
        }
        // `[off, off + 8)` with `off` in 8..16 is still to do, as is
        // everything below; the head covers `[0, 16)`.
        store(dst.add(off), load(src.add(off)));
        store(dst, h0);
        store(dst.add(8), h1);
    }
}

/// # Safety
///
/// `src..src + n` is readable, `dst..dst + n` writable, and they do not
/// overlap.
#[cfg_attr(target_os = "none", unsafe(no_mangle))]
pub unsafe extern "C" fn memcpy(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    let (d, s) = (dst.cast::<u8>(), src.cast::<u8>());
    // SAFETY: the caller's contract.
    unsafe {
        if n <= 16 {
            short(d, s, n);
        } else {
            forward(d, s, n);
        }
    }
    dst
}

/// # Safety
///
/// `src..src + n` is readable and `dst..dst + n` writable; they may
/// overlap.
#[cfg_attr(target_os = "none", unsafe(no_mangle))]
pub unsafe extern "C" fn memmove(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    let (d, s) = (dst.cast::<u8>(), src.cast::<u8>());
    // SAFETY: the caller's contract.
    unsafe {
        if n <= 16 {
            short(d, s, n);
        } else if (d as usize).wrapping_sub(s as usize) >= n {
            // `dst` below `src`, or past its end: upwards is safe.
            forward(d, s, n);
        } else {
            backward(d, s, n);
        }
    }
    dst
}

/// # Safety
///
/// `dst..dst + n` is writable.
#[cfg_attr(target_os = "none", unsafe(no_mangle))]
pub unsafe extern "C" fn memset(dst: *mut c_void, c: c_int, n: usize) -> *mut c_void {
    let d = dst.cast::<u8>();
    // `memset` stores `c` converted to `unsigned char`.
    let [byte, ..] = c.to_le_bytes();
    let half = u32::from_ne_bytes([byte; 4]);
    let word = u64::from_ne_bytes([byte; 8]);
    // SAFETY: the caller's contract; each access lies within the range.
    unsafe {
        match n {
            STRING_MIN.. => {
                asm!("rep stosb", inout("rcx") n => _, inout("rdi") d => _,
                    in("al") byte, options(nostack, preserves_flags));
            }
            8.. => {
                let mut off = 0;
                while off < n - 8 {
                    store(d.add(off), word);
                    off += 8;
                }
                store(d.add(n - 8), word);
            }
            4.. => {
                d.cast::<u32>().write_unaligned(half);
                d.add(n - 4).cast::<u32>().write_unaligned(half);
            }
            _ => {
                for i in 0..n {
                    d.add(i).write_volatile(byte);
                }
            }
        }
    }
    dst
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::vec::Vec;

    use super::*;

    fn pattern(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 + 3).to_le_bytes()[0]).collect()
    }

    /// Every length to past the string threshold, at every alignment,
    /// against a byte-by-byte copy; the bytes around stay untouched.
    #[test]
    fn copies_match_bytewise() {
        for n in 0..STRING_MIN + 40 {
            for (da, sa) in [(0, 0), (1, 0), (0, 3), (5, 7), (7, 1)] {
                let src = pattern(n + sa + 16);
                let mut dst = std::vec![0xeeu8; n + da + 16];
                let mut want = dst.clone();
                want[da + 8..da + 8 + n].copy_from_slice(&src[sa + 8..sa + 8 + n]);
                // SAFETY: both ranges are inside their vectors.
                unsafe {
                    memcpy(
                        dst.as_mut_ptr().add(da + 8).cast(),
                        src.as_ptr().add(sa + 8).cast(),
                        n,
                    );
                }
                assert_eq!(dst, want, "memcpy n={n} da={da} sa={sa}");
            }
        }
    }

    /// Overlapping moves in both directions, at every distance.
    #[test]
    fn moves_handle_overlap() {
        for n in 0..STRING_MIN + 40 {
            for shift in [1usize, 2, 7, 8, 9, 15, 16, 17, 40] {
                for up in [false, true] {
                    let mut buf = pattern(n + shift + 32);
                    let (from, to) = if up { (8, 8 + shift) } else { (8 + shift, 8) };
                    let mut want = buf.clone();
                    want.copy_within(from..from + n, to);
                    let p = buf.as_mut_ptr();
                    // SAFETY: both ranges are inside the vector.
                    unsafe { memmove(p.add(to).cast(), p.add(from).cast(), n) };
                    assert_eq!(buf, want, "memmove n={n} shift={shift} up={up}");
                }
            }
        }
    }

    #[test]
    fn fills_match_bytewise() {
        for n in 0..STRING_MIN + 40 {
            for a in [0, 1, 3, 7] {
                let mut buf = std::vec![0x11u8; n + a + 16];
                let mut want = buf.clone();
                want[a + 8..a + 8 + n].fill(0xa5);
                // SAFETY: the range is inside the vector.
                unsafe { memset(buf.as_mut_ptr().add(a + 8).cast(), 0x1a5, n) };
                assert_eq!(buf, want, "memset n={n} a={a}");
            }
        }
    }
}
