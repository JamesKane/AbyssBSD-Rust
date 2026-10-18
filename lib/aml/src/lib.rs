// SPDX-License-Identifier: BSD-2-Clause
//! An ACPI Machine Language interpreter (ACPI 6.5 §5.3, §19, §20).
//!
//! The design runs in four layers, each usable on its own:
//!
//! 1. **Parse**: a bounds-checked cursor reads AML with every length checked
//!    against the innermost enclosing package, so no corrupt `PkgLength`
//!    can reach outside its scope.
//! 2. **Namespace**: a tree of named objects in an arena addressed by
//!    generational [`NodeId`]s, with the spec's upward search rule for
//!    single-segment names. Several tables (the DSDT, then SSDTs) load into
//!    one namespace.
//! 3. **Interpreter**: [`Interpreter`] executes AML directly from table
//!    bytes. Loading a table runs its module-level code; method bodies are
//!    kept as byte ranges and parsed when invoked. Values are plain owned
//!    data ([`Value`]); references name storage locations.
//! 4. **Host**: operation regions, timing, notifications, and the global
//!    lock go through the [`Host`] trait, which the kernel implements and
//!    host tests fake.
//!
//! On top sit [`resource`] (decoding `_CRS`-style templates) and the device
//! queries on [`Interpreter`] (`_HID`, `_CID`, `_UID`, `_ADR`, `_STA`,
//! `_CRS`, `_PRT`, `_DEP`) that the ACPI bus uses.
//!
//! AML is firmware input and untrusted. Malformed or hostile AML produces an
//! [`Error`], never a panic: recursion, method calls, `While` loops, and
//! object sizes are all bounded (see [`Limits`]).
//!
//! Behavior reference: `sys/dev/acpica/` (how FreeBSD configures and uses
//! its interpreter: slack mode, `_OSI`, `_REV`), and the observable
//! behavior of `sys/contrib/dev/acpica/` where the specification is loose.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod device;
pub mod host;
mod interp;
pub mod name;
mod namespace;
pub mod resource;
mod stream;
mod value;

use alloc::string::String;

pub use device::{DepRef, PrtEntry, PrtSource, Uid, eisa_id, eisa_id_to_string};
pub use host::{
    AccessWidth, BufferTransfer, GpioAccess, Host, HostError, PciAddress, RegionAccess, RegionSpace,
};
pub use interp::{
    Config, FREEBSD_OSI, Interpreter, Limits, LoadReport, LoadWarning, MethodInfo,
    PowerResourceInfo, ProcessorInfo,
};
pub use name::{AmlPath, NameSeg, NameString};
pub use namespace::NodeId;
pub use value::{IntWidth, ObjectType, Reference, Value};

/// Why AML could not be loaded or evaluated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The AML ended in the middle of a term.
    UnexpectedEnd,
    /// A package length is malformed or reaches outside its scope.
    BadPkgLength,
    /// A malformed name.
    InvalidName,
    /// An opcode that is not valid here (extended opcodes as `0x5Bxx`).
    InvalidOpcode(u16),
    /// A field holds a value the spec does not allow.
    InvalidValue(&'static str),
    /// A name that AML referred to does not exist.
    Undefined(String),
    /// A lookup by the caller found nothing.
    NotFound,
    /// A named object with this name already exists.
    AlreadyExists,
    /// An operand has the wrong type.
    TypeMismatch,
    /// The target cannot be stored to.
    InvalidTarget,
    /// An index past the end of a package, buffer, or string.
    IndexOutOfBounds,
    /// A buffer field or `CreateField` reaches past its buffer.
    BufferLimit,
    /// A field access reaches past its operation region.
    RegionLimit,
    /// A GPIO or serial-bus field has no `Connection`.
    NoConnection,
    /// Division or modulo by zero.
    DivideByZero,
    /// A `While` loop ran past its iteration or time limit.
    LoopLimit,
    /// Nesting too deep: past [`Limits::max_depth`] levels or
    /// [`Limits::stack_budget`] bytes of stack.
    DepthLimit,
    /// Method calls nested deeper than [`Limits::max_call_depth`].
    CallDepth,
    /// A name that is not a method was invoked.
    NotAMethod,
    /// More arguments than a method can take.
    TooManyArgs,
    /// An uninitialized local or argument was read (outside slack mode).
    Uninitialized,
    /// A mutex was acquired below the current sync level.
    MutexOrder,
    /// A mutex that is not held was released.
    MutexNotOwned,
    /// A reference outlived what it refers to.
    DanglingReference,
    /// An object larger than [`Limits::max_object_size`], or allocation
    /// failed.
    NoMemory,
    /// A host operation failed.
    Host(HostError),
    /// A feature this interpreter does not provide.
    Unsupported(&'static str),
    /// A table header, length, or checksum is wrong.
    BadTable(&'static str),
    /// A resource template is malformed.
    BadResource(&'static str),
    /// An interpreter invariant failed (a bug, reported rather than
    /// panicking).
    Internal,
}

#[cfg(any(test, feature = "asm"))]
pub mod asm;
#[cfg(test)]
mod tests;
