// SPDX-License-Identifier: BSD-2-Clause
//! `memcpy` and `memset` for the arm64 kernel, replacing the
//! `compiler_builtins` ones (which are weak).
//!
//! Those move one 8-byte word per loop iteration. Signal frames, contexts,
//! FPU images, and the kernel's structures are copied all the time, and on
//! the Orange Pi 6 Plus `memcpy` was a quarter of a signal's delivery. Here
//! the bulk of a copy or a fill is 64 bytes per iteration, as four pairs
//! of general registers (`LDP`/`STP`): the kernel is soft-float, and the
//! FP/SIMD registers are the user's.
//!
//! Like the generic versions, these make no unaligned access, so they are
//! safe on Device memory (a driver may copy into a shared-memory mailbox):
//! the destination is aligned first, and a source that is then not aligned
//! with it is read as aligned words, shifted together.
//!
//! The moves are inline assembly, so the compiler cannot recognize the
//! loops as a copy and call back into these functions. `memmove`, `memcmp`
//! and `bcmp` stay `compiler_builtins`'.
//!
//! The symbols are defined only for the kernel's target; host tests run
//! the same code under its Rust names, with plain loads and stores.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

use core::ffi::{c_int, c_void};

/// The moves: aligned loads and stores of bytes, words, and pairs of
/// words. Each `p` is aligned to its size, and the access lies in a range
/// the caller may touch.
#[cfg(target_arch = "aarch64")]
mod mv {
    use core::arch::asm;

    #[inline(always)]
    pub(crate) unsafe fn ld1(p: *const u8) -> u8 {
        let v: u32;
        // SAFETY: the caller's contract.
        unsafe {
            asm!("ldrb {v:w}, [{p}]", p = in(reg) p, v = out(reg) v, options(nostack, readonly, preserves_flags))
        };
        v.to_le_bytes()[0]
    }

    #[inline(always)]
    pub(crate) unsafe fn st1(p: *mut u8, v: u8) {
        // SAFETY: the caller's contract.
        unsafe {
            asm!("strb {v:w}, [{p}]", p = in(reg) p, v = in(reg) u32::from(v), options(nostack, preserves_flags))
        };
    }

    #[inline(always)]
    pub(crate) unsafe fn ld8(p: *const u8) -> u64 {
        let v;
        // SAFETY: the caller's contract.
        unsafe {
            asm!("ldr {v}, [{p}]", p = in(reg) p, v = out(reg) v, options(nostack, readonly, preserves_flags))
        };
        v
    }

    #[inline(always)]
    pub(crate) unsafe fn st8(p: *mut u8, v: u64) {
        // SAFETY: the caller's contract.
        unsafe {
            asm!("str {v}, [{p}]", p = in(reg) p, v = in(reg) v, options(nostack, preserves_flags))
        };
    }

    /// 64 bytes from `s` to `d`, both 8-byte aligned.
    #[inline(always)]
    pub(crate) unsafe fn copy64(d: *mut u8, s: *const u8) {
        // SAFETY: the caller's contract.
        unsafe {
            asm!(
                "ldp {a}, {b}, [{s}]",
                "ldp {c}, {e}, [{s}, #16]",
                "stp {a}, {b}, [{d}]",
                "stp {c}, {e}, [{d}, #16]",
                "ldp {a}, {b}, [{s}, #32]",
                "ldp {c}, {e}, [{s}, #48]",
                "stp {a}, {b}, [{d}, #32]",
                "stp {c}, {e}, [{d}, #48]",
                s = in(reg) s, d = in(reg) d,
                a = out(reg) _, b = out(reg) _, c = out(reg) _, e = out(reg) _,
                options(nostack, preserves_flags),
            );
        }
    }

    /// `w` into the 64 bytes at `d`, 8-byte aligned.
    #[inline(always)]
    pub(crate) unsafe fn fill64(d: *mut u8, w: u64) {
        // SAFETY: the caller's contract.
        unsafe {
            asm!(
                "stp {w}, {w}, [{d}]",
                "stp {w}, {w}, [{d}, #16]",
                "stp {w}, {w}, [{d}, #32]",
                "stp {w}, {w}, [{d}, #48]",
                d = in(reg) d, w = in(reg) w,
                options(nostack, preserves_flags),
            );
        }
    }
}

/// The same moves for host tests, checking the alignment each promises.
#[cfg(not(target_arch = "aarch64"))]
mod mv {
    pub(crate) unsafe fn ld1(p: *const u8) -> u8 {
        // SAFETY: the caller's contract.
        unsafe { p.read_volatile() }
    }

    pub(crate) unsafe fn st1(p: *mut u8, v: u8) {
        // SAFETY: the caller's contract.
        unsafe { p.write_volatile(v) }
    }

    pub(crate) unsafe fn ld8(p: *const u8) -> u64 {
        assert!(p.addr().is_multiple_of(8), "unaligned load");
        // SAFETY: the caller's contract; aligned, as checked.
        unsafe { p.cast::<u64>().read_volatile() }
    }

    pub(crate) unsafe fn st8(p: *mut u8, v: u64) {
        assert!(p.addr().is_multiple_of(8), "unaligned store");
        // SAFETY: the caller's contract; aligned, as checked.
        unsafe { p.cast::<u64>().write_volatile(v) }
    }

    pub(crate) unsafe fn copy64(d: *mut u8, s: *const u8) {
        for i in (0..64).step_by(8) {
            // SAFETY: the caller's contract.
            unsafe { st8(d.add(i), ld8(s.add(i))) };
        }
    }

    pub(crate) unsafe fn fill64(d: *mut u8, w: u64) {
        for i in (0..64).step_by(8) {
            // SAFETY: the caller's contract.
            unsafe { st8(d.add(i), w) };
        }
    }
}

use mv::*;

/// Bytes from `d` up to its next 8-byte boundary (at most `n`).
fn head(d: *const u8, n: usize) -> usize {
    d.addr().wrapping_neg().rem_euclid(8).min(n)
}

/// # Safety
///
/// `src..src + n` is readable, `dst..dst + n` writable, and they do not
/// overlap.
#[cfg_attr(all(target_os = "none", target_arch = "aarch64"), unsafe(no_mangle))]
pub unsafe extern "C" fn memcpy(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    let (mut d, mut s) = (dst.cast::<u8>(), src.cast::<u8>());
    let mut n = n;
    // SAFETY: (throughout) the caller's contract; every access lies in the
    // two ranges, but for the misaligned case's whole-word source reads,
    // which stay in the aligned words holding the range's ends.
    unsafe {
        // Up to the destination's alignment, a byte at a time.
        for _ in 0..head(d, n) {
            st1(d, ld1(s));
            d = d.add(1);
            s = s.add(1);
            n -= 1;
        }
        if n >= 8 {
            let shift = s.addr() % 8;
            if shift == 0 {
                while n >= 64 {
                    copy64(d, s);
                    d = d.add(64);
                    s = s.add(64);
                    n -= 64;
                }
                while n >= 8 {
                    st8(d, ld8(s));
                    d = d.add(8);
                    s = s.add(8);
                    n -= 8;
                }
            } else {
                // Each destination word from two aligned source words.
                let bits = shift * 8;
                let mut w = s.sub(shift);
                let mut lo = ld8(w);
                while n >= 8 {
                    let hi = ld8(w.add(8));
                    st8(d, (lo >> bits) | (hi << (64 - bits)));
                    lo = hi;
                    w = w.add(8);
                    d = d.add(8);
                    s = s.add(8);
                    n -= 8;
                }
            }
        }
        for i in 0..n {
            st1(d.add(i), ld1(s.add(i)));
        }
    }
    dst
}

/// # Safety
///
/// `dst..dst + n` is writable.
#[cfg_attr(all(target_os = "none", target_arch = "aarch64"), unsafe(no_mangle))]
pub unsafe extern "C" fn memset(dst: *mut c_void, c: c_int, n: usize) -> *mut c_void {
    let mut d = dst.cast::<u8>();
    let mut n = n;
    // `memset` stores `c` converted to `unsigned char`.
    let [byte, ..] = c.to_le_bytes();
    let word = u64::from_ne_bytes([byte; 8]);
    // SAFETY: the caller's contract; every access lies in the range.
    unsafe {
        for _ in 0..head(d, n) {
            st1(d, byte);
            d = d.add(1);
            n -= 1;
        }
        while n >= 64 {
            fill64(d, word);
            d = d.add(64);
            n -= 64;
        }
        while n >= 8 {
            st8(d, word);
            d = d.add(8);
            n -= 8;
        }
        for i in 0..n {
            st1(d.add(i), byte);
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

    /// Every length to past a few loop rounds, at every relative
    /// alignment, against a byte-by-byte copy; the bytes around stay
    /// untouched. (The host's moves assert their alignment.)
    #[test]
    fn copies_match_bytewise() {
        for n in 0..300 {
            for da in 0..8 {
                for sa in 0..8 {
                    // 8-aligned storage, offsets from it.
                    let src: Vec<u64> = pattern((n + sa + 24) * 8 / 8 + 8)
                        .chunks(8)
                        .map(|c| c.iter().fold(0u64, |a, &b| a << 8 | u64::from(b)))
                        .collect();
                    let mut dst = std::vec![0xeeee_eeee_eeee_eeeeu64; n / 8 + 4];
                    let sp = src.as_ptr().cast::<u8>();
                    let dp = dst.as_mut_ptr().cast::<u8>();
                    // SAFETY: inside the vectors (n + 8 + 8 bytes each).
                    let (s, mut want) = unsafe {
                        (
                            core::slice::from_raw_parts(sp, src.len() * 8).to_vec(),
                            core::slice::from_raw_parts(dp, dst.len() * 8).to_vec(),
                        )
                    };
                    want[da + 8..da + 8 + n].copy_from_slice(&s[sa + 8..sa + 8 + n]);
                    // SAFETY: as above.
                    unsafe { memcpy(dp.add(da + 8).cast(), sp.add(sa + 8).cast(), n) };
                    // SAFETY: as above.
                    let got = unsafe { core::slice::from_raw_parts(dp, dst.len() * 8) };
                    assert_eq!(got, &want[..], "memcpy n={n} da={da} sa={sa}");
                }
            }
        }
    }

    #[test]
    fn fills_match_bytewise() {
        for n in 0..300 {
            for a in 0..8 {
                let mut buf = std::vec![0x1111_1111_1111_1111u64; n / 8 + 4];
                let p = buf.as_mut_ptr().cast::<u8>();
                let len = buf.len() * 8;
                // SAFETY: inside the vector.
                let mut want = unsafe { core::slice::from_raw_parts(p, len).to_vec() };
                want[a + 8..a + 8 + n].fill(0xa5);
                // SAFETY: inside the vector.
                unsafe { memset(p.add(a + 8).cast(), 0x1a5, n) };
                // SAFETY: as above.
                let got = unsafe { core::slice::from_raw_parts(p, len) };
                assert_eq!(got, &want[..], "memset n={n} a={a}");
            }
        }
    }
}
