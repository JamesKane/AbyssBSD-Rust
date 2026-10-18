// SPDX-License-Identifier: BSD-2-Clause
//! Memory shared with devices: plain old data only.
//!
//! A device can write any bits into memory it shares with the CPU (queue
//! entries, descriptors, rings), so the types laid over that memory must be
//! valid for every bit pattern and must hold nothing the kernel would trust:
//! no references or pointers, no `bool`, no `enum` (docs/DRIVER-MODEL.md
//! §8.4). They must also have no padding, so their layout is exactly the
//! hardware's.
//!
//! [`DeviceShared`] marks such types. It is implemented for the integer
//! types and for arrays of `DeviceShared` types; structs get it from
//! [`device_shared!`](crate::device_shared), which checks every field's type
//! and asserts at compile time that the struct has no padding. Values are
//! little-endian, as on every supported CPU.
//!
//! ```
//! regs::device_shared! {
//!     /// An NVMe completion queue entry (NVM Express Base 2.0, §4.2.1).
//!     pub struct Completion {
//!         pub dw0: u32,
//!         pub dw1: u32,
//!         pub sq_head: u16,
//!         pub sq_id: u16,
//!         pub cid: u16,
//!         pub status: u16,
//!     }
//! }
//! assert_eq!(size_of::<Completion>(), 16);
//! ```
//!
//! Padding is refused:
//!
//! ```compile_fail,E0080
//! regs::device_shared! {
//!     pub struct Padded { pub a: u8, pub b: u32 }
//! }
//! ```
//!
//! So are field types a device could set to an invalid value:
//!
//! ```compile_fail,E0277
//! regs::device_shared! {
//!     pub struct Flag { pub on: bool, pub rest: [u8; 3] }
//! }
//! ```

/// Plain old data that may be shared with a device: `#[repr(C)]` (or a
/// primitive), no padding, valid for every bit pattern, and free of
/// pointers.
///
/// # Safety
///
/// Implementors must satisfy every property above. Use
/// [`device_shared!`](crate::device_shared) rather than implementing it by
/// hand.
pub unsafe trait DeviceShared: Copy + 'static {}

// SAFETY: integers are valid for every bit pattern and have no padding.
unsafe impl DeviceShared for u8 {}
// SAFETY: as above.
unsafe impl DeviceShared for u16 {}
// SAFETY: as above.
unsafe impl DeviceShared for u32 {}
// SAFETY: as above.
unsafe impl DeviceShared for u64 {}
// SAFETY: as above.
unsafe impl DeviceShared for i8 {}
// SAFETY: as above.
unsafe impl DeviceShared for i16 {}
// SAFETY: as above.
unsafe impl DeviceShared for i32 {}
// SAFETY: as above.
unsafe impl DeviceShared for i64 {}
// SAFETY: an array of padding-free, all-bit-patterns-valid elements is
// itself both (array elements are laid out without gaps).
unsafe impl<T: DeviceShared, const N: usize> DeviceShared for [T; N] {}

/// Declares a `#[repr(C)]` struct that is [`DeviceShared`], deriving
/// `Clone` and `Copy`. Every field's type must be `DeviceShared`, and the
/// struct must have no padding; both are checked at compile time.
#[macro_export]
macro_rules! device_shared {
    (
        $(#[$m:meta])*
        $vis:vis struct $name:ident {
            $($(#[$fm:meta])* $fvis:vis $field:ident : $ty:ty),* $(,)?
        }
    ) => {
        $(#[$m])*
        #[repr(C)]
        #[derive(Clone, Copy)]
        $vis struct $name {
            $($(#[$fm])* $fvis $field: $ty),*
        }
        const _: () = {
            // Every field is device-shareable...
            const fn shareable<T: $crate::DeviceShared>() {}
            $(shareable::<$ty>();)*
            // ...and nothing sits between them: no padding.
            assert!(
                ::core::mem::size_of::<$name>() == 0 $(+ ::core::mem::size_of::<$ty>())*,
                "a DeviceShared struct must have no padding"
            );
        };
        // SAFETY: `repr(C)`, every field `DeviceShared`, and no padding
        // (checked just above).
        unsafe impl $crate::DeviceShared for $name {}
    };
}

#[cfg(test)]
mod tests {
    use super::DeviceShared;

    crate::device_shared! {
        /// An NVMe submission queue entry (NVM Express Base 2.0, §4.1).
        struct Submission {
            cdw0: u32,
            nsid: u32,
            cdw2: u32,
            cdw3: u32,
            mptr: u64,
            prp: [u64; 2],
            cdw10_15: [u32; 6],
        }
    }

    crate::device_shared! {
        struct Nested {
            entries: [Submission; 2],
            tail: u32,
            pad: [u8; 60],
        }
    }

    fn shareable<T: DeviceShared>() -> usize {
        size_of::<T>()
    }

    #[test]
    fn layouts() {
        assert_eq!(shareable::<Submission>(), 64);
        assert_eq!(shareable::<Nested>(), 192);
        assert_eq!(shareable::<[u16; 3]>(), 6);
        let s = Submission {
            cdw0: 1,
            nsid: 2,
            cdw2: 0,
            cdw3: 0,
            mptr: 0,
            prp: [0x1000, 0],
            cdw10_15: [0; 6],
        };
        let n = Nested {
            entries: [s; 2],
            tail: 1,
            pad: [0; 60],
        };
        assert_eq!(
            n.entries[1].prp[0] + u64::from(n.tail + n.entries[0].nsid),
            0x1003
        );
        assert_eq!(n.pad.len() + usize::from(n.entries[0].cdw0 == 1), 61);
    }
}
